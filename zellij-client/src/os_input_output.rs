use anyhow::{Context, Result};
use interprocess;
use zellij_utils::pane_size::Size;


use mio::{Events, Interest, Poll, Token};

#[cfg(unix)]
use ::{
    libc,
    nix,
    signal_hook,
    mio::unix::SourceFd,
    nix::{pty::Winsize, sys::termios},
    signal_hook::{consts::signal::*, iterator::Signals},
    std::os::unix::io::RawFd,
};

#[cfg(windows)]
use ::{
    mio::windows::NamedPipe,
    std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, RawHandle},
    windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::Console::{
            GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleInputA,
            SetConsoleMode, CONSOLE_SCREEN_BUFFER_INFO, COORD, FOCUS_EVENT, FOCUS_EVENT_RECORD,
            INPUT_RECORD, INPUT_RECORD_0, SMALL_RECT,
        },
    },
    zellij_utils::ipc::IpcSocketStream,
    zellij_utils::windows_utils::named_pipe,
};
use std::io::prelude::*;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Arc, Mutex, TryLockError};
use std::{io, thread, time};
use zellij_utils::{
    data::Palette,
    errors::ErrorContext,
    ipc::{ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg},
    shared::default_palette,
};

#[cfg(not(windows))]
const SIGWINCH_CB_THROTTLE_DURATION: time::Duration = time::Duration::from_millis(50);

const ENABLE_MOUSE_SUPPORT: &str =
    "\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1003h\u{1b}[?1015h\u{1b}[?1006h";
const DISABLE_MOUSE_SUPPORT: &str =
    "\u{1b}[?1006l\u{1b}[?1015l\u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l";

#[cfg(unix)]
fn into_raw_mode(pid: RawFd) {
    let mut tio = termios::tcgetattr(pid).expect("could not get terminal attribute");
    termios::cfmakeraw(&mut tio);
    match termios::tcsetattr(pid, termios::SetArg::TCSANOW, &tio) {
        Ok(_) => {},
        Err(e) => panic!("error {:?}", e),
    };
}

#[cfg(unix)]
fn unset_raw_mode(pid: RawFd, orig_termios: termios::Termios) -> Result<(), nix::Error> {
    termios::tcsetattr(pid, termios::SetArg::TCSANOW, &orig_termios)
}

pub enum HandleType {
    Stdin,
    Stdout,
    Stderr,
}

#[cfg(unix)]
pub(crate) fn get_terminal_size(handle_type: HandleType) -> Size {
    let fd = match handle_type {
        HandleType::Stdin => 0,
        HandleType::Stdout => 1,
        HandleType::Stderr => 2,
    };

    // TODO: do this with the nix ioctl
    use libc::ioctl;
    use libc::TIOCGWINSZ;

    let mut winsize = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // TIOCGWINSZ is an u32, but the second argument to ioctl is u64 on
    // some platforms. When checked on Linux, clippy will complain about
    // useless conversion.
    #[allow(clippy::useless_conversion)]
    unsafe {
        ioctl(fd, TIOCGWINSZ.into(), &mut winsize)
    };

    // fallback to default values when rows/cols == 0: https://github.com/zellij-org/zellij/issues/1551
    let rows = if winsize.ws_row != 0 {
        winsize.ws_row as usize
    } else {
        24
    };

    let cols = if winsize.ws_col != 0 {
        winsize.ws_col as usize
    } else {
        80
    };

    Size { rows, cols }
}

#[cfg(windows)]
pub(crate) fn get_terminal_size(handle_type: HandleType) -> Size {
    use std::mem;
    // TODO: handle other handle types, only stdout is supported for now

    let handle_type = match handle_type {
        HandleType::Stdin => windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
        HandleType::Stdout => windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
        HandleType::Stderr => windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
        _ => windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
    };

    let default_size = Size { rows: 24, cols: 80 };

    // get raw windows handle
    let handle = unsafe { GetStdHandle(handle_type) as RawHandle };

    // convert between windows_sys::Win32::Foundation::HANDLE and std::os::windows::raw::HANDLE
    let handle = handle as windows_sys::Win32::Foundation::HANDLE;

    if handle == INVALID_HANDLE_VALUE {
        return default_size;
    }

    let zc = COORD { X: 0, Y: 0 };
    let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = unsafe { mem::zeroed() };

    if unsafe { GetConsoleScreenBufferInfo(handle, &mut csbi) } == 0 {
        return default_size;
    }

    let cols = (csbi.srWindow.Right - csbi.srWindow.Left + 1) as usize;
    let rows = (csbi.srWindow.Bottom - csbi.srWindow.Top + 1) as usize;

    return Size { rows, cols };
}

#[derive(Clone)]
pub struct ClientOsInputOutput {
    #[cfg(unix)]
    orig_termios: Option<Arc<Mutex<termios::Termios>>>,
    send_instructions_to_server: Arc<Mutex<Option<IpcSenderWithContext<ClientToServerMsg>>>>,
    receive_instructions_from_server: Arc<Mutex<Option<IpcReceiverWithContext<ServerToClientMsg>>>>,
    reading_from_stdin: Arc<Mutex<Option<Vec<u8>>>>,
    session_name: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for ClientOsInputOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOsInputOutput").finish()
    }
}

/// The `ClientOsApi` trait represents an abstract interface to the features of an operating system that
/// Zellij client requires.
pub trait ClientOsApi: Send + Sync + std::fmt::Debug {
    /// Returns the size of the terminal associated to file descriptor `fd`.
    fn get_terminal_size(&self, handle_type: HandleType) -> Size;
    /// Set the terminal associated to file descriptor `fd` to
    /// [raw mode](https://en.wikipedia.org/wiki/Terminal_mode).
    #[cfg(unix)]
    fn set_raw_mode(&mut self, fd: RawFd);
    #[cfg(windows)]
    fn set_raw_mode(&mut self, handle_type: u32, enable_mode: u32, disable_mode: u32);
    /// Set the terminal associated to file descriptor `fd` to
    /// [cooked mode](https://en.wikipedia.org/wiki/Terminal_mode).
    #[cfg(unix)]
    fn unset_raw_mode(&self, fd: RawFd) -> Result<(), nix::Error>;
    #[cfg(windows)]
    fn unset_raw_mode(&self, fd: u32) -> Result<(), ()>; // TODO: wrap nix::Error
    /// Returns the writer that allows writing to standard output.
    fn get_stdout_writer(&self) -> Box<dyn io::Write>;
    /// Returns a BufReader that allows to read from STDIN line by line, also locks STDIN
    fn get_stdin_reader(&self) -> Box<dyn io::BufRead>;
    fn stdin_is_terminal(&self) -> bool {
        true
    }
    fn stdout_is_terminal(&self) -> bool {
        true
    }
    fn update_session_name(&mut self, new_session_name: String);
    /// Returns the raw contents of standard input.
    fn read_from_stdin(&mut self) -> Result<Vec<u8>, &'static str>;
    /// Returns a [`Box`] pointer to this [`ClientOsApi`] struct.
    fn box_clone(&self) -> Box<dyn ClientOsApi>;
    /// Sends a message to the server.
    fn send_to_server(&self, msg: ClientToServerMsg);
    /// Receives a message on client-side IPC channel
    // This should be called from the client-side router thread only.
    fn recv_from_server(&self) -> Option<(ServerToClientMsg, ErrorContext)>;
    fn handle_signals(&self, sigwinch_cb: Box<dyn Fn()>, quit_cb: Box<dyn Fn()>);
    /// Establish a connection with the server socket.
    fn connect_to_server(&self, path: &Path);
    fn load_palette(&self) -> Palette;
    fn enable_mouse(&self) -> Result<()>;
    fn disable_mouse(&self) -> Result<()>;
    // Repeatedly send action, until stdin is readable again
    fn stdin_poller(&self) -> StdinPoller;
    fn env_variable(&self, _name: &str) -> Option<String> {
        None
    }
}

impl ClientOsApi for ClientOsInputOutput {
    fn get_terminal_size(&self, handle_type: HandleType) -> Size {
        get_terminal_size(handle_type)
    }
    #[cfg(unix)]
    fn set_raw_mode(&mut self, fd: RawFd) {
        into_raw_mode(fd);
    }

    #[cfg(windows)]
    fn set_raw_mode(&mut self, handle: u32, enable_mode: u32, disable_mode: u32) {
        use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};

        let mut consolemode = 0 as u32;
        let fd = unsafe { GetStdHandle(handle) };
        unsafe { GetConsoleMode(fd, &mut consolemode) };
        consolemode = (consolemode & !disable_mode) | enable_mode;
        unsafe { SetConsoleMode(fd, consolemode) };
        unsafe { SetConsoleCP(65001) }; // Set Input CP to UTF8 (https://learn.microsoft.com/de-de/windows/win32/intl/code-page-identifiers)
        unsafe { SetConsoleOutputCP(65001) };
    }

    #[cfg(unix)]
    fn unset_raw_mode(&self, fd: RawFd) -> Result<(), nix::Error> {
        match &self.orig_termios {
            Some(orig_termios) => {
                let orig_termios = orig_termios.lock().unwrap();
                unset_raw_mode(fd, orig_termios.clone())
            },
            None => {
                log::warn!("trying to unset raw mode for a non-terminal session");
                Ok(())
            },
        }
    }
    #[cfg(windows)]
    fn unset_raw_mode(&self, _: u32) -> Result<(), ()> {
        Ok(())
    }
    fn box_clone(&self) -> Box<dyn ClientOsApi> {
        Box::new((*self).clone())
    }
    fn update_session_name(&mut self, new_session_name: String) {
        *self.session_name.lock().unwrap() = Some(new_session_name);
    }
    fn read_from_stdin(&mut self) -> Result<Vec<u8>, &'static str> {
        let session_name_at_calltime = { self.session_name.lock().unwrap().clone() };
        // here we wait for a lock in case another thread is holding stdin
        // this can happen for example when switching sessions, the old thread will only be
        // released once it sees input over STDIN
        //
        // when this happens, we detect in the other thread that our session is ended (by comparing
        // the session name at the beginning of the call and the one after we read from STDIN), and
        // so place what we read from STDIN inside a buffer (the "reading_from_stdin" on our state)
        // and release the lock
        //
        // then, another thread will see there's something in the buffer immediately as it acquires
        // the lock (without having to wait for STDIN itself) forward this buffer and proceed to
        // wait for the "real" STDIN net time it is called
        let mut buffered_bytes = self.reading_from_stdin.lock().unwrap();
        match buffered_bytes.take() {
            Some(buffered_bytes) => Ok(buffered_bytes),
            #[cfg(windows)]
            None => {
                let stdin = std::io::stdin().lock();
                let stdin_handle = stdin.as_handle();
                let mut buf = [INPUT_RECORD {
                    EventType: FOCUS_EVENT as u16,
                    Event: INPUT_RECORD_0 {
                        FocusEvent: FOCUS_EVENT_RECORD { bSetFocus: 0 },
                    },
                }; 128];
                let mut consumed: u32 = 0;
                let mut read_bytes = Vec::new();
                // SAFETY:
                // see https://learn.microsoft.com/en-us/windows/console/readconsoleinput for details
                // - hStdin: is the valid OS-Handle for Stdin of the current process so I expect the ACCESS_RIGHTS requirement to hold
                // - lpbuffer: is a valid pointer to an initialized Array of INPUT_RECORD
                // - nlength: is the length of the allocated Buffer
                // - lpnumberofeventsread: this out-parameter returns the number of elements consumed from the input queue
                if unsafe {
                    ReadConsoleInputA(
                        stdin_handle.as_raw_handle() as _,
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        (&mut consumed) as _,
                    )
                } != 0
                {
                    let buf: &[INPUT_RECORD] = &buf[..consumed as usize];
                    for event in buf {
                        let _ = match event.EventType as u32 {
                            windows_sys::Win32::System::Console::KEY_EVENT => {
                                // SAFETY: We just matched the tag
                                let args = unsafe { event.Event.KeyEvent };
                                // SAFETY: we called ReadConsoleInputA (the ascii version of the function)
                                let char = unsafe { args.uChar.AsciiChar };
                                if args.bKeyDown == 1 && char != 0 {
                                    read_bytes.push(char);
                                }
                                Ok(())
                            },
                            windows_sys::Win32::System::Console::MOUSE_EVENT => {
                                // SAFETY: we just matched the tag
                                let _args = unsafe { event.Event.MouseEvent };
                                // TODO implement mouse support. Alacritty does not forward any mouse events in my configuration
                                // So it is not clear how they arrive here and how they need to be treated.
                                Ok(())
                            },
                            windows_sys::Win32::System::Console::WINDOW_BUFFER_SIZE_EVENT => {
                                // SAFETY: we just matched the tag
                                let args = unsafe { event.Event.WindowBufferSizeEvent };
                                self.send_to_server(ClientToServerMsg::TerminalResize(Size {
                                    rows: args.dwSize.Y as usize,
                                    cols: args.dwSize.X as usize,
                                }));
                                Ok(())
                            },
                            windows_sys::Win32::System::Console::FOCUS_EVENT => {
                                // Discard as the payload is undocumented: https://learn.microsoft.com/en-us/windows/console/focus-event-record-str
                                Ok(())
                            },
                            windows_sys::Win32::System::Console::MENU_EVENT => {
                                // Discard as the payload is undocumented: https://learn.microsoft.com/en-us/windows/console/menu-event-record-str
                                Ok(())
                            },
                            x => Err(format!("Unknown Eventtype: {x}")),
                        };
                    }
                    let session_name_after_reading_from_stdin =
                        { self.session_name.lock().unwrap().clone() };
                    if session_name_at_calltime.is_some()
                        && session_name_at_calltime != session_name_after_reading_from_stdin
                    {
                        *buffered_bytes = Some(read_bytes);
                        Err("Session ended")
                    } else {
                        Ok(read_bytes)
                    }
                } else {
                    Err(std::io::Error::last_os_error().to_string().leak())
                }
            },
            #[cfg(not(windows))]
            None => {
                let stdin = std::io::stdin();
                let mut stdin = stdin.lock();
                let buffer = stdin.fill_buf().unwrap();
                let length = buffer.len();
                let read_bytes = Vec::from(buffer);
                stdin.consume(length);

                let session_name_after_reading_from_stdin =
                    { self.session_name.lock().unwrap().clone() };
                if session_name_at_calltime.is_some()
                    && session_name_at_calltime != session_name_after_reading_from_stdin
                {
                    *buffered_bytes = Some(read_bytes);
                    Err("Session ended")
                } else {
                    Ok(read_bytes)
                }
            },
        }
    }
    fn get_stdout_writer(&self) -> Box<dyn io::Write> {
        let stdout = ::std::io::stdout();
        Box::new(stdout)
    }

    fn get_stdin_reader(&self) -> Box<dyn io::BufRead> {
        let stdin = ::std::io::stdin();
        Box::new(stdin.lock())
    }

    fn stdin_is_terminal(&self) -> bool {
        let stdin = ::std::io::stdin();
        stdin.is_terminal()
    }

    fn stdout_is_terminal(&self) -> bool {
        let stdout = ::std::io::stdout();
        stdout.is_terminal()
    }

    fn send_to_server(&self, msg: ClientToServerMsg) {
        match self.send_instructions_to_server.lock().unwrap().as_mut() {
            Some(sender) => {
                let _ = sender.send(msg);
            },
            None => {
                log::warn!("Server not ready, dropping message.");
            },
        }
    }
    fn recv_from_server(&self) -> Option<(ServerToClientMsg, ErrorContext)> {
        log::info!("Receiving from Server");
        let receiver = self.receive_instructions_from_server.try_lock();
        let result = match receiver {
            Ok(mut receiver) => receiver
                .as_mut()
                .expect("Receiver should be initialized by now")
                .recv(),
            Err(TryLockError::WouldBlock) => {
                thread::sleep(std::time::Duration::from_secs_f32(0.1));
                None
            },
            Err(TryLockError::Poisoned(err)) => panic!("receiver has been poisoned"),
        };
        log::info!("{:?} received from server", &result);
        result
    }
    fn handle_signals(&self, sigwinch_cb: Box<dyn Fn()>, quit_cb: Box<dyn Fn()>) {
        #[cfg(unix)]
        {
            let mut sigwinch_cb_timestamp = time::Instant::now();
            let mut signals = Signals::new(&[SIGWINCH, SIGTERM, SIGINT, SIGQUIT, SIGHUP]).unwrap();
            for signal in signals.forever() {
                match signal {
                    SIGWINCH => {
                        // throttle sigwinch_cb calls, reduce excessive renders while resizing
                        if sigwinch_cb_timestamp.elapsed() < SIGWINCH_CB_THROTTLE_DURATION {
                            thread::sleep(SIGWINCH_CB_THROTTLE_DURATION);
                        }
                        sigwinch_cb_timestamp = time::Instant::now();
                        sigwinch_cb();
                    },
                    SIGTERM | SIGINT | SIGQUIT | SIGHUP => {
                        quit_cb();
                        break;
                    },
                    _ => unreachable!(),
                }
            }
        }
    }
    fn connect_to_server(&self, path: &Path) {
        let socket;
        loop {
            match IpcSocketStream::connect(path) {
                Ok(sock) => {
                    socket = sock;
                    break;
                },
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                },
            }
        }
        let receiver = IpcReceiverWithContext::new(socket);
        let sender = receiver.get_sender();
        *self.send_instructions_to_server.lock().unwrap() = Some(sender);
        *self.receive_instructions_from_server.lock().unwrap() = Some(receiver);
    }
    fn load_palette(&self) -> Palette {
        // this was removed because termbg doesn't release stdin in certain scenarios (we know of
        // windows terminal and FreeBSD): https://github.com/zellij-org/zellij/issues/538
        //
        // let palette = default_palette();
        // let timeout = std::time::Duration::from_millis(100);
        // if let Ok(rgb) = termbg::rgb(timeout) {
        //     palette.bg = PaletteColor::Rgb((rgb.r as u8, rgb.g as u8, rgb.b as u8));
        //     // TODO: also dynamically get all other colors from the user's terminal
        //     // this should be done in the same method (OSC ]11), but there might be other
        //     // considerations here, hence using the library
        // };
        default_palette()
    }
    fn enable_mouse(&self) -> Result<()> {
        let err_context = "failed to enable mouse mode";
        let mut stdout = self.get_stdout_writer();
        stdout
            .write_all(ENABLE_MOUSE_SUPPORT.as_bytes())
            .context(err_context)?;
        stdout.flush().context(err_context)?;
        Ok(())
    }

    fn disable_mouse(&self) -> Result<()> {
        let err_context = "failed to enable mouse mode";
        let mut stdout = self.get_stdout_writer();
        stdout
            .write_all(DISABLE_MOUSE_SUPPORT.as_bytes())
            .context(err_context)?;
        stdout.flush().context(err_context)?;
        Ok(())
    }

    fn stdin_poller(&self) -> StdinPoller {
        StdinPoller::default()
    }

    fn env_variable(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl Clone for Box<dyn ClientOsApi> {
    fn clone(&self) -> Box<dyn ClientOsApi> {
        self.box_clone()
    }
}
#[cfg(unix)]
pub fn get_client_os_input() -> Result<ClientOsInputOutput, nix::Error> {
    let current_termios = termios::tcgetattr(0).ok();
    let orig_termios = current_termios.map(|termios| Arc::new(Mutex::new(termios)));
    let reading_from_stdin = Arc::new(Mutex::new(None));
    Ok(ClientOsInputOutput {
        orig_termios,
        send_instructions_to_server: Arc::new(Mutex::new(None)),
        receive_instructions_from_server: Arc::new(Mutex::new(None)),
        reading_from_stdin,
        session_name: Arc::new(Mutex::new(None)),
    })
}

#[cfg(windows)]
pub fn get_client_os_input() -> Result<ClientOsInputOutput, ()> {
    Ok(ClientOsInputOutput {
        send_instructions_to_server: Arc::new(Mutex::new(None)),
        receive_instructions_from_server: Arc::new(Mutex::new(None)),
        reading_from_stdin: Arc::new(Mutex::new(None)),
        session_name: Arc::new(Mutex::new(None)),
    })
}

#[cfg(unix)]
pub fn get_cli_client_os_input() -> Result<ClientOsInputOutput, nix::Error> {
    let orig_termios = None; // not a terminal
    let reading_from_stdin = Arc::new(Mutex::new(None));
    Ok(ClientOsInputOutput {
        orig_termios,
        send_instructions_to_server: Arc::new(Mutex::new(None)),
        receive_instructions_from_server: Arc::new(Mutex::new(None)),
        reading_from_stdin,
        session_name: Arc::new(Mutex::new(None)),
    })
}

#[cfg(windows)]
pub fn get_cli_client_os_input() -> Result<ClientOsInputOutput, ()> {
    Ok(ClientOsInputOutput {
        send_instructions_to_server: Arc::new(Mutex::new(None)),
        receive_instructions_from_server: Arc::new(Mutex::new(None)),
        reading_from_stdin: Arc::new(Mutex::new(None)),
        session_name: Arc::new(Mutex::new(None)),
    })
}

pub const DEFAULT_STDIN_POLL_TIMEOUT_MS: u64 = 10;

pub struct StdinPoller {
    #[cfg(unix)]
    poll: Poll,
    #[cfg(unix)]
    events: Events,
    timeout: time::Duration,
}

impl StdinPoller {
    #[cfg(unix)]
    // use mio poll to check if stdin is readable without blocking
    pub fn ready(&mut self) -> bool {
        self.poll
            .poll(&mut self.events, Some(self.timeout))
            .expect("could not poll stdin for readiness");
        for event in &self.events {
            if event.token() == Token(0) && event.is_readable() {
                return true;
            }
        }
        false
    }

    #[cfg(windows)]
    pub fn ready(&mut self) -> bool {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::System::Console::{
            GetNumberOfConsoleInputEvents, STD_INPUT_HANDLE,
        };
        use windows_sys::Win32::System::Threading::WaitForSingleObjectEx;
        let mut pending_events = 0u32;
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };

        let success =
            unsafe { GetNumberOfConsoleInputEvents(stdin_handle, &mut pending_events as _) };
        if success == FALSE {
            log::error!("Could not query the number of input events from stdin");
            return false;
        }

        if pending_events > 0 {
            return true;
        } else {
            let result = unsafe {
                WaitForSingleObjectEx(
                    stdin_handle,
                    self.timeout
                        .as_millis()
                        .clamp(1, u32::MAX.into())
                        .try_into()
                        .unwrap(),
                    FALSE,
                )
            };
            match result {
                0 => return true,    // wait object is ready to fetch
                102 => return false, // timeout occured
                x => return false, // something else happened see https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobjectex#return-value
            }
        }
    }
}

impl Default for StdinPoller {
    #[cfg(unix)]
    fn default() -> Self {
        let stdin = 0;
        let mut stdin_fd = SourceFd(&stdin);
        let events = Events::with_capacity(128);
        let poll = Poll::new().unwrap();
        poll.registry()
            .register(&mut stdin_fd, Token(0), Interest::READABLE)
            .expect("could not create stdin poll");

        let timeout = time::Duration::from_millis(DEFAULT_STDIN_POLL_TIMEOUT_MS);

        Self {
            poll,
            events,
            timeout,
        }
    }

    #[cfg(windows)]
    fn default() -> Self {
        let timeout = time::Duration::from_millis(DEFAULT_STDIN_POLL_TIMEOUT_MS);

        Self { timeout }
    }
}
