#![cfg(x11_platform)]

mod activation;
mod atoms;
mod dnd;
mod event_processor;
pub mod ffi;
mod ime;
mod monitor;
pub mod util;
mod window;
mod xdisplay;

pub(crate) use self::{
    monitor::{MonitorHandle, VideoMode},
    window::UnownedWindow,
    xdisplay::XConnection,
};

pub use self::xdisplay::{XError, XNotSupported};

use calloop::channel::{channel, Channel, Event as ChanResult, Sender};
use calloop::generic::Generic;
use calloop::{Dispatcher, EventLoop as Loop};

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    ffi::CStr,
    fmt,
    mem::{self, MaybeUninit},
    ops::Deref,
    os::{
        raw::*,
        unix::io::{AsRawFd, RawFd},
    },
    ptr,
    rc::Rc,
    slice,
    sync::{mpsc, Arc, Weak},
    time::{Duration, Instant},
};

use libc::{self, setlocale, LC_CTYPE};

use atoms::*;
use raw_window_handle::{RawDisplayHandle, XlibDisplayHandle};

use x11rb::protocol::{
    xinput,
    xproto::{self, ConnectionExt},
};
use x11rb::x11_utils::X11Error as LogicalError;
use x11rb::{
    errors::{ConnectError, ConnectionError, IdsExhausted, ReplyError},
    xcb_ffi::ReplyOrIdError,
};

use self::{
    dnd::{Dnd, DndState},
    event_processor::EventProcessor,
    ime::{Ime, ImeCreationError, ImeReceiver, ImeRequest, ImeSender},
};
use super::common::xkb_state::KbdState;
use crate::{
    error::OsError as RootOsError,
    event::{Event, StartCause},
    event_loop::{ControlFlow, DeviceEvents, EventLoopClosed, EventLoopWindowTarget as RootELW},
    platform_impl::{
        platform::{sticky_exit_callback, WindowId},
        PlatformSpecificWindowBuilderAttributes,
    },
    window::WindowAttributes,
};

type X11Source = Generic<RawFd>;

pub struct EventLoopWindowTarget<T> {
    xconn: Arc<XConnection>,
    wm_delete_window: xproto::Atom,
    net_wm_ping: xproto::Atom,
    ime_sender: ImeSender,
    root: xproto::Window,
    ime: RefCell<Ime>,
    windows: RefCell<HashMap<WindowId, Weak<UnownedWindow>>>,
    redraw_sender: Sender<WindowId>,
    activation_sender: Sender<ActivationToken>,
    device_events: Cell<DeviceEvents>,
    _marker: ::std::marker::PhantomData<T>,
}

pub struct EventLoop<T: 'static> {
    event_loop: Loop<'static, EventLoopState<T>>,
    event_processor: EventProcessor<T>,
    user_sender: Sender<T>,
    target: Rc<RootELW<T>>,

    /// The current state of the event loop.
    state: EventLoopState<T>,

    /// Dispatcher for redraw events.
    redraw_dispatcher: Dispatcher<'static, Channel<WindowId>, EventLoopState<T>>,
}

type ActivationToken = (WindowId, crate::event_loop::AsyncRequestSerial);

struct EventLoopState<T> {
    /// Incoming user events.
    user_events: VecDeque<T>,

    /// Incoming redraw events.
    redraw_events: VecDeque<WindowId>,

    /// Incoming activation tokens.
    activation_tokens: VecDeque<ActivationToken>,
}

pub struct EventLoopProxy<T: 'static> {
    user_sender: Sender<T>,
}

impl<T: 'static> Clone for EventLoopProxy<T> {
    fn clone(&self) -> Self {
        EventLoopProxy {
            user_sender: self.user_sender.clone(),
        }
    }
}

impl<T: 'static> EventLoop<T> {
    pub(crate) fn new(xconn: Arc<XConnection>) -> EventLoop<T> {
        let root = xconn.default_root().root;
        let atoms = xconn.atoms();

        let wm_delete_window = atoms[WM_DELETE_WINDOW];
        let net_wm_ping = atoms[_NET_WM_PING];

        let dnd = Dnd::new(Arc::clone(&xconn))
            .expect("Failed to call XInternAtoms when initializing drag and drop");

        let (ime_sender, ime_receiver) = mpsc::channel();
        let (ime_event_sender, ime_event_receiver) = mpsc::channel();
        // Input methods will open successfully without setting the locale, but it won't be
        // possible to actually commit pre-edit sequences.
        unsafe {
            // Remember default locale to restore it if target locale is unsupported
            // by Xlib
            let default_locale = setlocale(LC_CTYPE, ptr::null());
            setlocale(LC_CTYPE, b"\0".as_ptr() as *const _);

            // Check if set locale is supported by Xlib.
            // If not, calls to some Xlib functions like `XSetLocaleModifiers`
            // will fail.
            let locale_supported = (xconn.xlib.XSupportsLocale)() == 1;
            if !locale_supported {
                let unsupported_locale = setlocale(LC_CTYPE, ptr::null());
                warn!(
                    "Unsupported locale \"{}\". Restoring default locale \"{}\".",
                    CStr::from_ptr(unsupported_locale).to_string_lossy(),
                    CStr::from_ptr(default_locale).to_string_lossy()
                );
                // Restore default locale
                setlocale(LC_CTYPE, default_locale);
            }
        }
        let ime = RefCell::new({
            let result = Ime::new(Arc::clone(&xconn), ime_event_sender);
            if let Err(ImeCreationError::OpenFailure(ref state)) = result {
                panic!("Failed to open input method: {state:#?}");
            }
            result.expect("Failed to set input method destruction callback")
        });

        let randr_event_offset = xconn
            .select_xrandr_input(root as ffi::Window)
            .expect("Failed to query XRandR extension");

        let xi2ext = unsafe {
            let mut ext = XExtension::default();

            let res = (xconn.xlib.XQueryExtension)(
                xconn.display,
                b"XInputExtension\0".as_ptr() as *const c_char,
                &mut ext.opcode,
                &mut ext.first_event_id,
                &mut ext.first_error_id,
            );

            if res == ffi::False {
                panic!("X server missing XInput extension");
            }

            ext
        };

        let xkbext = {
            let mut ext = XExtension::default();

            let res = unsafe {
                (xconn.xlib.XkbQueryExtension)(
                    xconn.display,
                    &mut ext.opcode,
                    &mut ext.first_event_id,
                    &mut ext.first_error_id,
                    &mut 1,
                    &mut 0,
                )
            };

            if res == ffi::False {
                panic!("X server missing XKB extension");
            }

            // Enable detectable auto repeat.
            let mut supported = 0;
            unsafe {
                (xconn.xlib.XkbSetDetectableAutoRepeat)(xconn.display, 1, &mut supported);
            }
            if supported == 0 {
                warn!("Detectable auto repeart is not supported");
            }

            ext
        };

        unsafe {
            let mut xinput_major_ver = ffi::XI_2_Major;
            let mut xinput_minor_ver = ffi::XI_2_Minor;
            if (xconn.xinput2.XIQueryVersion)(
                xconn.display,
                &mut xinput_major_ver,
                &mut xinput_minor_ver,
            ) != ffi::Success as std::os::raw::c_int
            {
                panic!(
                    "X server has XInput extension {xinput_major_ver}.{xinput_minor_ver} but does not support XInput2",
                );
            }
        }

        xconn.update_cached_wm_info(root);

        // Create an event loop.
        let event_loop =
            Loop::<EventLoopState<T>>::try_new().expect("Failed to initialize the event loop");
        let handle = event_loop.handle();

        // Create the X11 event dispatcher.
        let source = X11Source::new(
            xconn.xcb_connection().as_raw_fd(),
            calloop::Interest::READ,
            calloop::Mode::Level,
        );
        handle
            .insert_source(source, |_, _, _| Ok(calloop::PostAction::Continue))
            .expect("Failed to register the X11 event dispatcher");

        // Create a channel for sending user events.
        let (user_sender, user_channel) = channel();
        handle
            .insert_source(user_channel, |ev, _, state| {
                if let ChanResult::Msg(user) = ev {
                    state.user_events.push_back(user);
                }
            })
            .expect("Failed to register the user event channel with the event loop");

        // Create a channel for handling redraw requests.
        let (redraw_sender, redraw_channel) = channel();

        // Create a channel for sending activation tokens.
        let (activation_token_sender, activation_token_channel) = channel();

        // Create a dispatcher for the redraw channel such that we can dispatch it independent of the
        // event loop.
        let redraw_dispatcher =
            Dispatcher::<_, EventLoopState<T>>::new(redraw_channel, |ev, _, state| {
                if let ChanResult::Msg(window_id) = ev {
                    state.redraw_events.push_back(window_id);
                }
            });
        handle
            .register_dispatcher(redraw_dispatcher.clone())
            .expect("Failed to register the redraw event channel with the event loop");

        // Create a dispatcher for the activation token channel such that we can dispatch it
        // independent of the event loop.
        let activation_tokens =
            Dispatcher::<_, EventLoopState<T>>::new(activation_token_channel, |ev, _, state| {
                if let ChanResult::Msg(token) = ev {
                    state.activation_tokens.push_back(token);
                }
            });
        handle
            .register_dispatcher(activation_tokens.clone())
            .expect("Failed to register the activation token channel with the event loop");

        let kb_state =
            KbdState::from_x11_xkb(xconn.xcb_connection().get_raw_xcb_connection()).unwrap();

        let window_target = EventLoopWindowTarget {
            ime,
            root,
            windows: Default::default(),
            _marker: ::std::marker::PhantomData,
            ime_sender,
            xconn,
            wm_delete_window,
            net_wm_ping,
            redraw_sender,
            activation_sender: activation_token_sender,
            device_events: Default::default(),
        };

        // Set initial device event filter.
        window_target.update_listen_device_events(true);

        let target = Rc::new(RootELW {
            p: super::EventLoopWindowTarget::X(window_target),
            _marker: ::std::marker::PhantomData,
        });

        let event_processor = EventProcessor {
            target: target.clone(),
            dnd,
            devices: Default::default(),
            randr_event_offset,
            ime_receiver,
            ime_event_receiver,
            xi2ext,
            xkbext,
            kb_state,
            num_touch: 0,
            held_key_press: None,
            first_touch: None,
            active_window: None,
            is_composing: false,
        };

        // Register for device hotplug events
        // (The request buffer is flushed during `init_device`)
        get_xtarget(&target)
            .xconn
            .select_xinput_events(
                root,
                ffi::XIAllDevices as _,
                x11rb::protocol::xinput::XIEventMask::HIERARCHY,
            )
            .expect_then_ignore_error("Failed to register for XInput2 device hotplug events");

        get_xtarget(&target)
            .xconn
            .select_xkb_events(
                0x100, // Use the "core keyboard device"
                ffi::XkbNewKeyboardNotifyMask | ffi::XkbStateNotifyMask,
            )
            .unwrap();

        event_processor.init_device(ffi::XIAllDevices);

        EventLoop {
            event_loop,
            event_processor,
            user_sender,
            target,
            redraw_dispatcher,
            state: EventLoopState {
                user_events: VecDeque::new(),
                redraw_events: VecDeque::new(),
                activation_tokens: VecDeque::new(),
            },
        }
    }

    pub fn create_proxy(&self) -> EventLoopProxy<T> {
        EventLoopProxy {
            user_sender: self.user_sender.clone(),
        }
    }

    pub(crate) fn window_target(&self) -> &RootELW<T> {
        &self.target
    }

    pub fn run_return<F>(&mut self, mut callback: F) -> i32
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        struct IterationResult {
            deadline: Option<Instant>,
            timeout: Option<Duration>,
            wait_start: Instant,
        }
        fn single_iteration<T, F>(
            this: &mut EventLoop<T>,
            control_flow: &mut ControlFlow,
            cause: &mut StartCause,
            callback: &mut F,
        ) -> IterationResult
        where
            F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
        {
            sticky_exit_callback(
                crate::event::Event::NewEvents(*cause),
                &this.target,
                control_flow,
                callback,
            );

            // NB: For consistency all platforms must emit a 'resumed' event even though X11
            // applications don't themselves have a formal suspend/resume lifecycle.
            if *cause == StartCause::Init {
                sticky_exit_callback(
                    crate::event::Event::Resumed,
                    &this.target,
                    control_flow,
                    callback,
                );
            }

            // Process all pending events
            this.drain_events(callback, control_flow);

            // Empty activation tokens.
            while let Some((window_id, serial)) = this.state.activation_tokens.pop_front() {
                let token = this
                    .event_processor
                    .with_window(window_id.0 as xproto::Window, |window| {
                        window.generate_activation_token()
                    });

                match token {
                    Some(Ok(token)) => sticky_exit_callback(
                        crate::event::Event::WindowEvent {
                            window_id: crate::window::WindowId(window_id),
                            event: crate::event::WindowEvent::ActivationTokenDone {
                                serial,
                                token: crate::window::ActivationToken::_new(token),
                            },
                        },
                        &this.target,
                        control_flow,
                        callback,
                    ),
                    Some(Err(e)) => {
                        log::error!("Failed to get activation token: {}", e);
                    }
                    None => {}
                }
            }

            // Empty the user event buffer
            {
                while let Some(event) = this.state.user_events.pop_front() {
                    sticky_exit_callback(
                        crate::event::Event::UserEvent(event),
                        &this.target,
                        control_flow,
                        callback,
                    );
                }
            }
            // send MainEventsCleared
            {
                sticky_exit_callback(
                    crate::event::Event::MainEventsCleared,
                    &this.target,
                    control_flow,
                    callback,
                );
            }

            // Quickly dispatch all redraw events to avoid buffering them.
            while let Ok(event) = this.redraw_dispatcher.as_source_mut().try_recv() {
                this.state.redraw_events.push_back(event);
            }

            // Empty the redraw requests
            {
                let mut windows = HashSet::new();

                // Empty the channel.

                while let Some(window_id) = this.state.redraw_events.pop_front() {
                    windows.insert(window_id);
                }

                for window_id in windows {
                    let window_id = crate::window::WindowId(window_id);
                    sticky_exit_callback(
                        Event::RedrawRequested(window_id),
                        &this.target,
                        control_flow,
                        callback,
                    );
                }
            }
            // send RedrawEventsCleared
            {
                sticky_exit_callback(
                    crate::event::Event::RedrawEventsCleared,
                    &this.target,
                    control_flow,
                    callback,
                );
            }

            let start = Instant::now();
            let (deadline, timeout);

            match control_flow {
                ControlFlow::ExitWithCode(_) => {
                    return IterationResult {
                        wait_start: start,
                        deadline: None,
                        timeout: None,
                    };
                }
                ControlFlow::Poll => {
                    *cause = StartCause::Poll;
                    deadline = None;
                    timeout = Some(Duration::from_millis(0));
                }
                ControlFlow::Wait => {
                    *cause = StartCause::WaitCancelled {
                        start,
                        requested_resume: None,
                    };
                    deadline = None;
                    timeout = None;
                }
                ControlFlow::WaitUntil(wait_deadline) => {
                    *cause = StartCause::ResumeTimeReached {
                        start,
                        requested_resume: *wait_deadline,
                    };
                    timeout = if *wait_deadline > start {
                        Some(*wait_deadline - start)
                    } else {
                        Some(Duration::from_millis(0))
                    };
                    deadline = Some(*wait_deadline);
                }
            }

            IterationResult {
                wait_start: start,
                deadline,
                timeout,
            }
        }

        let mut control_flow = ControlFlow::default();
        let mut cause = StartCause::Init;

        // run the initial loop iteration
        let mut iter_result = single_iteration(self, &mut control_flow, &mut cause, &mut callback);

        let exit_code = loop {
            if let ControlFlow::ExitWithCode(code) = control_flow {
                break code;
            }
            let has_pending = self.event_processor.poll()
                || !self.state.user_events.is_empty()
                || !self.state.redraw_events.is_empty();
            if !has_pending {
                // Wait until
                if let Err(error) = self
                    .event_loop
                    .dispatch(iter_result.timeout, &mut self.state)
                    .map_err(std::io::Error::from)
                {
                    break error.raw_os_error().unwrap_or(1);
                }

                if control_flow == ControlFlow::Wait {
                    // We don't go straight into executing the event loop iteration, we instead go
                    // to the start of this loop and check again if there's any pending event. We
                    // must do this because during the execution of the iteration we sometimes wake
                    // the calloop waker, and if the waker is already awaken before we call poll(),
                    // then poll doesn't block, but it returns immediately. This caused the event
                    // loop to run continuously even if the control_flow was `Wait`
                    continue;
                }
            }

            let wait_cancelled = iter_result
                .deadline
                .map_or(false, |deadline| Instant::now() < deadline);

            if wait_cancelled {
                cause = StartCause::WaitCancelled {
                    start: iter_result.wait_start,
                    requested_resume: iter_result.deadline,
                };
            }

            iter_result = single_iteration(self, &mut control_flow, &mut cause, &mut callback);
        };

        callback(
            crate::event::Event::LoopDestroyed,
            &self.target,
            &mut control_flow,
        );
        exit_code
    }

    pub fn run<F>(mut self, callback: F) -> !
    where
        F: 'static + FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let exit_code = self.run_return(callback);
        ::std::process::exit(exit_code);
    }

    fn drain_events<F>(&mut self, callback: &mut F, control_flow: &mut ControlFlow)
    where
        F: FnMut(Event<'_, T>, &RootELW<T>, &mut ControlFlow),
    {
        let target = &self.target;
        let mut xev = MaybeUninit::uninit();
        let wt = get_xtarget(&self.target);

        while unsafe { self.event_processor.poll_one_event(xev.as_mut_ptr()) } {
            let mut xev = unsafe { xev.assume_init() };
            self.event_processor.process_event(&mut xev, |event| {
                sticky_exit_callback(
                    event,
                    target,
                    control_flow,
                    &mut |event, window_target, control_flow| {
                        if let Event::RedrawRequested(crate::window::WindowId(wid)) = event {
                            wt.redraw_sender.send(wid).unwrap();
                        } else {
                            callback(event, window_target, control_flow);
                        }
                    },
                );
            });
        }
    }
}

pub(crate) fn get_xtarget<T>(target: &RootELW<T>) -> &EventLoopWindowTarget<T> {
    match target.p {
        super::EventLoopWindowTarget::X(ref target) => target,
        #[cfg(wayland_platform)]
        _ => unreachable!(),
    }
}

impl<T> EventLoopWindowTarget<T> {
    /// Returns the `XConnection` of this events loop.
    #[inline]
    pub(crate) fn x_connection(&self) -> &Arc<XConnection> {
        &self.xconn
    }

    pub fn set_listen_device_events(&self, allowed: DeviceEvents) {
        self.device_events.set(allowed);
    }

    /// Update the device event based on window focus.
    pub fn update_listen_device_events(&self, focus: bool) {
        let device_events = self.device_events.get() == DeviceEvents::Always
            || (focus && self.device_events.get() == DeviceEvents::WhenFocused);

        let mut mask = xinput::XIEventMask::from(0u32);
        if device_events {
            mask = xinput::XIEventMask::RAW_MOTION
                | xinput::XIEventMask::RAW_BUTTON_PRESS
                | xinput::XIEventMask::RAW_BUTTON_RELEASE
                | xinput::XIEventMask::RAW_KEY_PRESS
                | xinput::XIEventMask::RAW_KEY_RELEASE;
        }

        self.xconn
            .select_xinput_events(self.root, ffi::XIAllMasterDevices as _, mask)
            .expect_then_ignore_error("Failed to update device event filter");
    }

    pub fn raw_display_handle(&self) -> raw_window_handle::RawDisplayHandle {
        let mut display_handle = XlibDisplayHandle::empty();
        display_handle.display = self.xconn.display as *mut _;
        display_handle.screen = self.xconn.default_screen_index() as c_int;
        RawDisplayHandle::Xlib(display_handle)
    }
}

impl<T: 'static> EventLoopProxy<T> {
    pub fn send_event(&self, event: T) -> Result<(), EventLoopClosed<T>> {
        self.user_sender
            .send(event)
            .map_err(|e| EventLoopClosed(e.0))
    }
}

struct DeviceInfo<'a> {
    xconn: &'a XConnection,
    info: *const ffi::XIDeviceInfo,
    count: usize,
}

impl<'a> DeviceInfo<'a> {
    fn get(xconn: &'a XConnection, device: c_int) -> Option<Self> {
        unsafe {
            let mut count = 0;
            let info = (xconn.xinput2.XIQueryDevice)(xconn.display, device, &mut count);
            xconn.check_errors().ok()?;

            if info.is_null() || count == 0 {
                None
            } else {
                Some(DeviceInfo {
                    xconn,
                    info,
                    count: count as usize,
                })
            }
        }
    }
}

impl<'a> Drop for DeviceInfo<'a> {
    fn drop(&mut self) {
        assert!(!self.info.is_null());
        unsafe { (self.xconn.xinput2.XIFreeDeviceInfo)(self.info as *mut _) };
    }
}

impl<'a> Deref for DeviceInfo<'a> {
    type Target = [ffi::XIDeviceInfo];
    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.info, self.count) }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(c_int);

impl DeviceId {
    #[allow(unused)]
    pub const unsafe fn dummy() -> Self {
        DeviceId(0)
    }
}

pub(crate) struct Window(Arc<UnownedWindow>);

impl Deref for Window {
    type Target = UnownedWindow;
    #[inline]
    fn deref(&self) -> &UnownedWindow {
        &self.0
    }
}

impl Window {
    pub(crate) fn new<T>(
        event_loop: &EventLoopWindowTarget<T>,
        attribs: WindowAttributes,
        pl_attribs: PlatformSpecificWindowBuilderAttributes,
    ) -> Result<Self, RootOsError> {
        let window = Arc::new(UnownedWindow::new(event_loop, attribs, pl_attribs)?);
        event_loop
            .windows
            .borrow_mut()
            .insert(window.id(), Arc::downgrade(&window));
        Ok(Window(window))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let window = self.deref();
        let xconn = &window.xconn;

        if let Ok(c) = xconn
            .xcb_connection()
            .destroy_window(window.id().0 as xproto::Window)
        {
            c.ignore_error();
        }
    }
}

/// Generic sum error type for X11 errors.
#[derive(Debug)]
pub enum X11Error {
    /// An error from the Xlib library.
    Xlib(XError),

    /// An error that occurred while trying to connect to the X server.
    Connect(ConnectError),

    /// An error that occurred over the connection medium.
    Connection(ConnectionError),

    /// An error that occurred logically on the X11 end.
    X11(LogicalError),

    /// The XID range has been exhausted.
    XidsExhausted(IdsExhausted),

    /// Got `null` from an Xlib function without a reason.
    UnexpectedNull(&'static str),

    /// Got an invalid activation token.
    InvalidActivationToken(Vec<u8>),
}

impl fmt::Display for X11Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            X11Error::Xlib(e) => write!(f, "Xlib error: {}", e),
            X11Error::Connect(e) => write!(f, "X11 connection error: {}", e),
            X11Error::Connection(e) => write!(f, "X11 connection error: {}", e),
            X11Error::XidsExhausted(e) => write!(f, "XID range exhausted: {}", e),
            X11Error::X11(e) => write!(f, "X11 error: {:?}", e),
            X11Error::UnexpectedNull(s) => write!(f, "Xlib function returned null: {}", s),
            X11Error::InvalidActivationToken(s) => write!(
                f,
                "Invalid activation token: {}",
                std::str::from_utf8(s).unwrap_or("<invalid utf8>")
            ),
        }
    }
}

impl std::error::Error for X11Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            X11Error::Xlib(e) => Some(e),
            X11Error::Connect(e) => Some(e),
            X11Error::Connection(e) => Some(e),
            X11Error::XidsExhausted(e) => Some(e),
            _ => None,
        }
    }
}

impl From<XError> for X11Error {
    fn from(e: XError) -> Self {
        X11Error::Xlib(e)
    }
}

impl From<ConnectError> for X11Error {
    fn from(e: ConnectError) -> Self {
        X11Error::Connect(e)
    }
}

impl From<ConnectionError> for X11Error {
    fn from(e: ConnectionError) -> Self {
        X11Error::Connection(e)
    }
}

impl From<LogicalError> for X11Error {
    fn from(e: LogicalError) -> Self {
        X11Error::X11(e)
    }
}

impl From<ReplyError> for X11Error {
    fn from(value: ReplyError) -> Self {
        match value {
            ReplyError::ConnectionError(e) => e.into(),
            ReplyError::X11Error(e) => e.into(),
        }
    }
}

impl From<ime::ImeContextCreationError> for X11Error {
    fn from(value: ime::ImeContextCreationError) -> Self {
        match value {
            ime::ImeContextCreationError::XError(e) => e.into(),
            ime::ImeContextCreationError::Null => Self::UnexpectedNull("XOpenIM"),
        }
    }
}

impl From<ReplyOrIdError> for X11Error {
    fn from(value: ReplyOrIdError) -> Self {
        match value {
            ReplyOrIdError::ConnectionError(e) => e.into(),
            ReplyOrIdError::X11Error(e) => e.into(),
            ReplyOrIdError::IdsExhausted => Self::XidsExhausted(IdsExhausted),
        }
    }
}

/// The underlying x11rb connection that we are using.
type X11rbConnection = x11rb::xcb_ffi::XCBConnection;

/// Type alias for a void cookie.
type VoidCookie<'a> = x11rb::cookie::VoidCookie<'a, X11rbConnection>;

/// Extension trait for `Result<VoidCookie, E>`.
trait CookieResultExt {
    /// Unwrap the send error and ignore the result.
    fn expect_then_ignore_error(self, msg: &str);
}

impl<'a, E: fmt::Debug> CookieResultExt for Result<VoidCookie<'a>, E> {
    fn expect_then_ignore_error(self, msg: &str) {
        self.expect(msg).ignore_error()
    }
}

/// XEvents of type GenericEvent store their actual data in an XGenericEventCookie data structure. This is a wrapper to
/// extract the cookie from a GenericEvent XEvent and release the cookie data once it has been processed
struct GenericEventCookie<'a> {
    xconn: &'a XConnection,
    cookie: ffi::XGenericEventCookie,
}

impl<'a> GenericEventCookie<'a> {
    fn from_event(xconn: &XConnection, event: ffi::XEvent) -> Option<GenericEventCookie<'_>> {
        unsafe {
            let mut cookie: ffi::XGenericEventCookie = From::from(event);
            if (xconn.xlib.XGetEventData)(xconn.display, &mut cookie) == ffi::True {
                Some(GenericEventCookie { xconn, cookie })
            } else {
                None
            }
        }
    }
}

impl<'a> Drop for GenericEventCookie<'a> {
    fn drop(&mut self) {
        unsafe {
            (self.xconn.xlib.XFreeEventData)(self.xconn.display, &mut self.cookie);
        }
    }
}

#[derive(Debug, Default, Copy, Clone)]
struct XExtension {
    opcode: c_int,
    first_event_id: c_int,
    first_error_id: c_int,
}

fn mkwid(w: xproto::Window) -> crate::window::WindowId {
    crate::window::WindowId(crate::platform_impl::platform::WindowId(w as _))
}
fn mkdid(w: c_int) -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::X(DeviceId(w)))
}

#[derive(Debug)]
struct Device {
    _name: String,
    scroll_axes: Vec<(i32, ScrollAxis)>,
    // For master devices, this is the paired device (pointer <-> keyboard).
    // For slave devices, this is the master.
    attachment: c_int,
}

#[derive(Debug, Copy, Clone)]
struct ScrollAxis {
    increment: f64,
    orientation: ScrollOrientation,
    position: f64,
}

#[derive(Debug, Copy, Clone)]
enum ScrollOrientation {
    Vertical,
    Horizontal,
}

impl Device {
    fn new(info: &ffi::XIDeviceInfo) -> Self {
        let name = unsafe { CStr::from_ptr(info.name).to_string_lossy() };
        let mut scroll_axes = Vec::new();

        if Device::physical_device(info) {
            // Identify scroll axes
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                if class._type == ffi::XIScrollClass {
                    let info = unsafe {
                        mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIScrollClassInfo>(class)
                    };
                    scroll_axes.push((
                        info.number,
                        ScrollAxis {
                            increment: info.increment,
                            orientation: match info.scroll_type {
                                ffi::XIScrollTypeHorizontal => ScrollOrientation::Horizontal,
                                ffi::XIScrollTypeVertical => ScrollOrientation::Vertical,
                                _ => unreachable!(),
                            },
                            position: 0.0,
                        },
                    ));
                }
            }
        }

        let mut device = Device {
            _name: name.into_owned(),
            scroll_axes,
            attachment: info.attachment,
        };
        device.reset_scroll_position(info);
        device
    }

    fn reset_scroll_position(&mut self, info: &ffi::XIDeviceInfo) {
        if Device::physical_device(info) {
            for class_ptr in Device::classes(info) {
                let class = unsafe { &**class_ptr };
                if class._type == ffi::XIValuatorClass {
                    let info = unsafe {
                        mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIValuatorClassInfo>(class)
                    };
                    if let Some(&mut (_, ref mut axis)) = self
                        .scroll_axes
                        .iter_mut()
                        .find(|&&mut (axis, _)| axis == info.number)
                    {
                        axis.position = info.value;
                    }
                }
            }
        }
    }

    #[inline]
    fn physical_device(info: &ffi::XIDeviceInfo) -> bool {
        info._use == ffi::XISlaveKeyboard
            || info._use == ffi::XISlavePointer
            || info._use == ffi::XIFloatingSlave
    }

    #[inline]
    fn classes(info: &ffi::XIDeviceInfo) -> &[*const ffi::XIAnyClassInfo] {
        unsafe {
            slice::from_raw_parts(
                info.classes as *const *const ffi::XIAnyClassInfo,
                info.num_classes as usize,
            )
        }
    }
}
