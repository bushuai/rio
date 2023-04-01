mod control;
pub mod handler;

use crate::crosswords::Crosswords;
use crate::event::sync::FairMutex;
use crate::event::EventListener;
use mio::unix::pipe::{Receiver, Sender};
use std::os::fd::AsRawFd;

use crate::event::{Msg, RioEvent};
use mio::{Events, Interest, Token};
use std::borrow::Cow;
use std::collections::VecDeque;

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Instant;

use std::io::{ErrorKind, Write};

const PIPE_RECV: Token = Token(0);
const PIPE_SEND: Token = Token(1);
const PIPE_PTY: Token = Token(2);

const READ_BUFFER_SIZE: usize = 0x10_0000;
/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

pub type MsgSender<T> = std::sync::mpsc::Sender<T>;
pub type MsgReceiver<T> = std::sync::mpsc::Receiver<T>;

fn unbounded<T>() -> (MsgSender<T>, MsgReceiver<T>) {
    // TODO: Implemente Sync for mio Events
    // tokio::sync::mpsc::channel::<T>(READ_BUFFER_SIZE)
    std::sync::mpsc::channel::<T>()
}

pub struct Machine<T: teletypewriter::EventedPty, U: EventListener> {
    sender: MsgSender<Msg>,
    receiver: MsgReceiver<Msg>,
    mio_sender: mio::unix::pipe::Sender,
    mio_receiver: mio::unix::pipe::Receiver,
    waker: Arc<mio::Waker>,
    pty: T,
    poll: mio::Poll,
    terminal: Arc<FairMutex<Crosswords<U>>>,
    event_proxy: U,
}

#[derive(Default)]
pub struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: handler::ParserProcessor,
}

impl State {
    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing {
            source: c,
            written: 0,
        }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

impl<T, U> Machine<T, U>
where
    T: teletypewriter::EventedPty + Send + 'static,
    U: EventListener + Send + 'static,
{
    pub fn new(
        terminal: Arc<FairMutex<Crosswords<U>>>,
        pty: T,
        event_proxy: U,
    ) -> Result<Machine<T, U>, Box<dyn std::error::Error>> {
        let (mut sender, mut receiver) = unbounded::<Msg>();
        let poll = mio::Poll::new()?;

        let event_loop_waker = Arc::new(mio::Waker::new(poll.registry(), PIPE_SEND)?);

        let (mut mio_sender, mut mio_receiver) = mio::unix::pipe::new()?;

        poll.registry().register(&mut mio_receiver, PIPE_RECV, Interest::READABLE)?;
        poll.registry().register(&mut mio_sender, PIPE_SEND, Interest::WRITABLE)?;

        Ok(Machine {
            sender,
            receiver,
            mio_sender,
            mio_receiver,
            waker: event_loop_waker,
            poll,
            pty,
            terminal,
            event_proxy,
        })
    }

    #[inline]
    fn pty_read(&mut self, state: &mut State, buf: &mut [u8]) -> io::Result<()> {
        let mut unprocessed = 0;
        let mut processed = 0;

        // Reserve the next terminal lock for PTY reading.
        let _terminal_lease = Some(self.terminal.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                // This is received on Windows/macOS when no more data is readable from the PTY.
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // Go back to mio if we're caught up on parsing and the PTY would block.
                        if unprocessed == 0 {
                            break;
                        }
                    }
                    _ => return Err(err),
                },
            }

            // Attempt to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.terminal.try_lock_unfair() {
                    // Force block if we are at the buffer size limit.
                    None if unprocessed >= READ_BUFFER_SIZE => {
                        self.terminal.lock_unfair()
                    }
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Parse the incoming bytes.
            for byte in &buf[..unprocessed] {
                state.parser.advance(&mut **terminal, *byte);
            }

            processed += unprocessed;
            unprocessed = 0;

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        // Queue terminal redraw unless all processed bytes were synchronized.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.event_proxy.send_event(RioEvent::Wakeup);
        }

        Ok(())
    }

    fn should_keep_alive(&mut self, state: &mut State) -> bool {
        println!("lendo");
        while let Ok(msg) = self.receiver.try_recv() {
            println!("msg chegou: {:?}", msg);
            match msg {
                Msg::Input(input) => {
                    println!("input {:?}", input);
                    state.write_list.push_back(input);
                },
                Msg::Resize(window_size) => {},
                Msg::Shutdown => return false,
            }
        }

        println!("aki {:?}", state.write_list);

        true
    }

    /// Returns a `bool` indicating whether or not the event loop should continue running.
    #[inline]
    fn channel_event(&mut self, token: mio::Token, state: &mut State) -> bool {
        // if self.drain_recv_channel(state) {
        return self.should_keep_alive(state);
        // }

        // self.poll
        //     .registry()
        //     .reregister(&mut self.receiver, token, Interest::READABLE)
            // .unwrap();

    }

    #[inline]
    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();

        'write_many: while let Some(mut current) = state.take_current() {
            'write_one: loop {
                match self.pty.writer().write(current.remaining_bytes()) {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    }
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    }
                    Err(err) => {
                        state.set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                                break 'write_many
                            }
                            _ => return Err(err),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn channel(&self) -> MsgSender<Msg> {
        self.sender.clone()
    }

    pub fn channel_mio(&mut self) -> &mut Sender {
        self.mio_sender.by_ref()
    }

    pub fn spawn(mut self) {
        tokio::spawn(async move {
            let mut state = State::default();
            let mut buf = [0u8; READ_BUFFER_SIZE];

            let mut tokens = PIPE_PTY;
            // let register = self
            //     .poll
            //     .registry()
            //     .register(&mut self.mio_receiver, tokens, Interest::READABLE)
            //     .unwrap();

            // Register TTY through EventedRW interface.
            self.pty.register(&self.poll, tokens).unwrap();

            let mut events = Events::with_capacity(1024);
            let mut channel_token = 0;

            'event_loop: loop {
                // Wakeup the event loop when a synchronized update timeout was reached.
                let sync_timeout = state.parser.sync_timeout();
                let timeout =
                    sync_timeout.map(|st| st.saturating_duration_since(Instant::now()));

                if let Err(err) = self.poll.poll(&mut events, timeout) {
                    match err.kind() {
                        ErrorKind::Interrupted => continue,
                        _ => panic!("EventLoop polling error: {err:?}"),
                    }
                }

                // Handle synchronized update timeout.
                if events.is_empty() {
                    state.parser.stop_sync(&mut *self.terminal.lock());
                    self.event_proxy.send_event(RioEvent::Wakeup);
                    continue;
                }

                for event in events.iter() {
                    println!(
                        "{:?} {:?}",
                        event,
                        event.token()
                    );

                    match event.token() {
                        PIPE_RECV if event.is_read_closed() => {
                            // Detected that the sender was dropped.
                            break 'event_loop;
                        },
                        token if token == PIPE_SEND => {
                            if !self.should_keep_alive(&mut state)
                            {
                                break 'event_loop;
                            }
                        }
                        token if token == self.pty.child_event_token() => {
                            // if let Some(teletypewriter::ChildEvent::Exited) =
                            //     self.pty.next_child_event()
                            // {
                            self.pty_read(&mut state, &mut buf);
                            self.event_proxy.send_event(RioEvent::Wakeup);
                            // break 'event_loop;
                            // }
                        }

                        token
                            if token == self.pty.read_token()
                                || token == self.pty.write_token() =>
                        {
                            println!("caiu aki");
                            #[cfg(unix)]
                            // if UnixReady::from(event.readiness()).is_hup() {
                            //     // Don't try to do I/O on a dead PTY.
                            //     continue;
                            // }
                            if event.is_readable() {
                                if let Err(err) = self.pty_read(&mut state, &mut buf) {
                                    // On Linux, a `read` on the master side of a PTY can fail
                                    // with `EIO` if the client side hangs up.  In that case,
                                    // just loop back round for the inevitable `Exited` event.
                                    // This sucks, but checking the process is either racy or
                                    // blocking.
                                    #[cfg(target_os = "linux")]
                                    if err.raw_os_error() == Some(libc::EIO) {
                                        continue;
                                    }

                                    println!(
                                        "Error reading from PTY in event loop: {}",
                                        err
                                    );
                                    break 'event_loop;
                                }
                            }

                            if event.is_writable() {
                                if let Err(err) = self.pty_write(&mut state) {
                                    println!(
                                        "Error writing to PTY in event loop: {}",
                                        err
                                    );
                                    break 'event_loop;
                                }
                            }
                        }
                        _ => (),
                    }
                }

                // Register write interest if necessary.
                let mut interest = Interest::READABLE;
                if state.needs_write() {
                    interest.add(Interest::WRITABLE);
                }
                // Reregister with new interest.
                // self.pty
                //     .reregister(&self.poll, interest)
                //     .unwrap();
            }

            // The evented instances are not dropped here so deregister them explicitly.
            let _ = self.poll.registry().deregister(&mut self.mio_receiver);
            let _ = self.pty.deregister(&self.poll);

            (self, state)
        });
    }
}