mod error;
use error::{Result, ResultExt, CliResult, Error, ErrorKind};

mod cli;

mod config;
use config::Config;

mod device;
use device::{Device, Event, RawEvent, OpMode, LatchState, ConnectionState};

mod service;
use service::{Service, DetachState};

use std::convert::TryFrom;
use std::{cell::RefCell, rc::Rc};
use std::time::Duration;
use std::process::Command;
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

use tokio::prelude::*;
use tokio::runtime::current_thread::Runtime;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_signal::unix::{Signal, SIGINT, SIGTERM};
use tokio_process::CommandExt;

use slog::{Logger, trace, debug, info, warn, error, crit, o};



fn logger(config: &Config) -> Logger {
    use slog::{Drain};

    let decorator = slog_term::TermDecorator::new()
        .build();

    let drain = slog_term::FullFormat::new(decorator)
        .use_original_order()
        .build()
        .filter_level(config.log.level.into())
        .fuse();

    let drain = std::sync::Mutex::new(drain)
        .fuse();

    Logger::root(drain, o!())
}

fn main() -> CliResult {
    let matches = cli::app().get_matches();

    let config = match matches.value_of("config") {
        Some(path) => Config::load_file(path)?,
        None       => Config::load()?,
    };

    let logger = logger(&config);

    let mut runtime = Runtime::new().context(ErrorKind::Runtime)?;
    let device: Rc<_> = Device::open()?.into();

    // set-up task-queue for external processes
    let (queue_tx, queue_rx) = tokio::sync::mpsc::channel(32);

    // dbus service
    let connection: Rc<_> = dbus::Connection::get_private(dbus::BusType::System)
        .context(ErrorKind::DBusService)?
        .into();

    let serv: Rc<_> = service::build(logger.clone(), connection, device.clone())?.into();
    let serv_task = serv.task(&mut runtime).context(ErrorKind::DBusService)?;

    // event handler
    let event_task = setup_event_task(logger.clone(), config, serv.clone(), device.clone(), queue_tx)?;

    // process queue handler
    let process_task = setup_process_task(queue_rx).shared();
    let shutdown_task = process_task.clone().map(|_| ());

    // shutdown handler and main task
    let signal = setup_shutdown_signal(logger.clone(), shutdown_task);
    let main_task = signal
        .select(event_task).map(|_| ()).map_err(|(e, _)| panic!(e))
        .select(serv_task).map(|_| ()).map_err(|(e, _)| panic!(e));

    // only critical errors will reach this point
    let log = logger.clone();
    let main_task = main_task.map_err(move |e| {
        panic_with_critical_error(&log, &e);
    });

    let log = logger.clone();
    let process_task = process_task.map(|_| ()).map_err(move |e| {
        panic_with_critical_error(&log, &e);
    });

    // make sure the device-mode in the service is up to date
    let init_task = future::lazy(move || {
        serv.set_device_mode(device.commands().get_opmode()?);
        Ok(())
    });

    let log = logger.clone();
    let init_task = init_task.map_err(move |e| {
        panic_with_critical_error(&log, &e);
    });

    debug!(logger, "running...");
    runtime.spawn(main_task);
    runtime.spawn(process_task);
    runtime.spawn(init_task);
    runtime.run().unwrap();

    Ok(())
}


fn setup_shutdown_signal<F>(log: Logger, shutdown_task: F) -> impl Future<Item=(), Error=Error>
where
    F: Future + 'static,
    <F as Future>::Error: std::fmt::Display,
{
    let signal = {
        let sigint = Signal::new(SIGINT).flatten_stream();
        let sigterm = Signal::new(SIGTERM).flatten_stream();

        sigint.select(sigterm).into_future()
            .map_err(|(e, _)| Error::with(e, ErrorKind::Runtime))
    };

    signal.map(move |(sig, next)| {
        info!(log, "shutting down...");

        // actual shutdown code provided via shutdown_task: wait for completion
        let l = log.clone();
        let task = shutdown_task.map(|_| ()).map_err(move |e| {
            error!(l, "error while terminating: {}", e);
        });

        // on second signal: terminate, no questions asked
        let l = log.clone();
        let term = next.into_future().then(move |_| -> std::result::Result<(), ()> {
            info!(l, "terminating...");
            std::process::exit(128 + sig.unwrap_or(SIGINT))
        });

        let task = task.select(term)
            .map(|_| ()).map_err(|_| ());

        tokio::runtime::current_thread::spawn(task)
    })
}

fn setup_event_task(log: Logger, config: Config, service: Rc<Service>,
                    device: Rc<Device>, task_queue_tx: Sender<BoxedTask>)
    -> Result<impl Future<Item=(), Error=Error>>
{
    let events = device.events()?.map_err(Error::from);

    let mut handler = EventHandler::new(log, config, service, device, task_queue_tx);
    let task = events.for_each(move |evt| {
        handler.handle(evt)
    });

    Ok(task)
}

fn setup_process_task(task_queue_rx: Receiver<BoxedTask>)
    -> impl Future<Item=(), Error=Error>
{
    // an error here (sender closed while trying to receive) is a failure in program logic
    let task = task_queue_rx.map_err(|e| panic!(e));

    task.for_each(|task| {
        task
    })
}


type BoxedTask = Box<dyn Future<Item=(), Error=Error>>;

struct EventHandler {
    log: Logger,
    config: Config,
    service: Rc<Service>,
    device: Rc<Device>,
    state: Rc<RefCell<State>>,
    task_queue_tx: Sender<BoxedTask>,
    ignore_request: u32,
}

impl EventHandler {
    fn new(log: Logger, config: Config, service: Rc<Service>, device: Rc<Device>,
           task_queue_tx: Sender<BoxedTask>)
        -> Self
    {
        let state = Rc::new(RefCell::new(State::Normal));

        EventHandler {
            log,
            config,
            service,
            task_queue_tx,
            device,
            state,
            ignore_request: 0,
        }
    }


    fn handle(&mut self, evt: RawEvent) -> Result<()> {
        trace!(self.log, "received event"; "event" => ?evt);

        match Event::try_from(evt) {
            Ok(Event::OpModeChange { mode }) => {
                self.on_opmode_change(mode)
            },
            Ok(Event::ConectionChange { state, .. }) => {
                self.on_connection_change(state)
            },
            Ok(Event::LatchStateChange { state }) => {
                self.on_latch_state_change(state)
            },
            Ok(Event::DetachRequest) => {
                self.on_detach_request()
            },
            Ok(Event::DetachError { err }) => {
                self.on_detach_error(err)
            },
            Err(evt) => {
                warn!(self.log, "unhandled event";
                    "type" => evt.typ,  "code" => evt.code,
                    "arg0" => evt.arg0, "arg1" => evt.arg1
                );

                Ok(())
            },
        }
    }


    fn on_opmode_change(&mut self, mode: OpMode) -> Result<()> {
        debug!(self.log, "device mode changed"; "mode" => ?mode);
        self.service.set_device_mode(mode);
        Ok(())
    }

    fn on_latch_state_change(&mut self, state: LatchState) -> Result<()> {
        debug!(self.log, "latch-state changed"; "state" => ?state);

        if state == LatchState::Open {
            self.service.signal_detach_state_change(DetachState::DetachReady)
        }

        Ok(())
    }

    fn on_connection_change(&mut self, connection: ConnectionState) -> Result<()> {
        debug!(self.log, "clipboard connection changed"; "state" => ?connection);

        let state = *self.state.borrow();
        match (state, connection) {
            (State::Detaching, ConnectionState::Disconnected) => {
                *self.state.borrow_mut() = State::Normal;
                self.service.signal_detach_state_change(DetachState::DetachCompleted);
                debug!(self.log, "detachment procedure completed");
                Ok(())
            },
            (State::Normal, ConnectionState::Connected) => {
                { *self.state.borrow_mut() = State::Attaching; }
                self.schedule_task_attach()
            },
            _ => {
                error!(self.log, "invalid state"; "state" => ?(*self.state.borrow(), state));
                Ok(())
            },
        }
    }

    fn on_detach_request(&mut self) -> Result<()> {
        if self.ignore_request > 0 {
            self.ignore_request -= 1;
            return Ok(());
        }

        let state = *self.state.borrow();
        match state {
            State::Normal => {
                debug!(self.log, "clipboard detach requested");
                *self.state.borrow_mut() = State::Detaching;
                self.schedule_task_detach()
            },
            State::Detaching => {
                debug!(self.log, "clipboard detach-abort requested");
                *self.state.borrow_mut() = State::Aborting;
                self.service.signal_detach_state_change(DetachState::DetachAborted);
                self.schedule_task_detach_abort()
            },
            State::Aborting | State::Attaching => {
                self.ignore_request += 1;
                self.device.commands().latch_request()
            },
        }
    }

    fn on_detach_error(&mut self, err: u8) -> Result<()> {
        if err == 0x02 {
            debug!(self.log, "detachment procedure: timed out");
        } else {
            error!(self.log, "unknown error event"; "code" => err);
        }

        if *self.state.borrow() == State::Detaching {
            *self.state.borrow_mut() = State::Aborting;
            self.schedule_task_detach_abort()
        } else {
            Ok(())
        }
    }


    fn schedule_task_attach(&mut self) -> Result<()> {
        let log = self.log.clone();
        let task = future::lazy(move || {
            debug!(log, "subprocess: delaying attach process");
            Ok(())
        });

        let delay = Duration::from_millis((self.config.delay.attach * 1000.0) as _);
        let task = task.and_then(move |_| {
            // any error here (shutdown, at_capacity) is a failure in program logic
            tokio_timer::sleep(delay).map_err(|e| panic!(e))
        });

        let handler = self.config.handler.attach.as_ref();
        if let Some(ref path) = handler {
            let mut command = Command::new(path);
            command.current_dir(&self.config.dir);

            let log = self.log.clone();
            let task = task.and_then(move |_| {
                debug!(log, "subprocess: attach started");
                command.output_async()
            });

            let log = self.log.clone();
            let state = self.state.clone();
            let service = self.service.clone();
            let task = task.map_err(|e| Error::with(e, ErrorKind::Process));
            let task = task.and_then(move |output| {
                debug!(log, "subprocess: attach finished");
                log_process_output(&log, &output);

                *state.borrow_mut() = State::Normal;
                service.signal_detach_state_change(DetachState::AttachCompleted);
                Ok(())
            });

            self.schedule_process_task(Box::new(task))

        } else {
            let log = self.log.clone();
            let state = self.state.clone();
            let service = self.service.clone();
            let task = task.map_err(|e| Error::with(e, ErrorKind::Runtime));
            let task = task.and_then(move |_| {
                debug!(log, "subprocess: no attach handler executable");

                *state.borrow_mut() = State::Normal;
                service.signal_detach_state_change(DetachState::AttachCompleted);
                Ok(())
            });

            self.schedule_process_task(Box::new(task))
        }
    }

    fn schedule_task_detach(&mut self) -> Result<()> {
        let handler = self.config.handler.detach.as_ref();

        if let Some(ref path) = handler {
            let mut command = Command::new(path);
            command.current_dir(&self.config.dir);
            command.env("EXIT_DETACH_COMMENCE", "0");
            command.env("EXIT_DETACH_ABORT", "1");

            let log = self.log.clone();
            let task = future::lazy(move || {
                debug!(log, "subprocess: detach started");
                command.output_async()
            });

            let log = self.log.clone();
            let state = self.state.clone();
            let device = self.device.clone();
            let task = task.map_err(|e| Error::with(e, ErrorKind::Process));
            let task = task.and_then(move |output| {
                debug!(log, "subprocess: detach finished");
                log_process_output(&log, &output);

                if *state.borrow() == State::Detaching {
                    if output.status.success() {
                        debug!(log, "commencing detach, opening latch");
                        device.commands().latch_open()?;
                    } else {
                        info!(log, "aborting detach");
                        device.commands().latch_request()?;
                    }
                } else {
                    debug!(log, "state changed during detachment, not opening latch");
                }

                Ok(())
            });

            self.schedule_process_task(Box::new(task))

        } else {
            let log = self.log.clone();
            let state = self.state.clone();
            let device = self.device.clone();
            let task = future::lazy(move || {
                debug!(log, "subprocess: no detach handler executable");

                if *state.borrow() == State::Detaching {
                    debug!(log, "commencing detach, opening latch");
                    device.commands().latch_open()?;
                } else {
                    debug!(log, "state changed during detachment, not opening latch");
                }

                Ok(())
            });

            self.schedule_process_task(Box::new(task))
        }
    }

    fn schedule_task_detach_abort(&mut self) -> Result<()> {
        let handler = self.config.handler.detach_abort.as_ref();

        if let Some(ref path) = handler {
            let mut command = Command::new(path);
            command.current_dir(&self.config.dir);

            let log = self.log.clone();
            let task = future::lazy(move || {
                debug!(log, "subprocess: detach_abort started");
                command.output_async()
            });

            let log = self.log.clone();
            let state = self.state.clone();
            let task = task.map_err(|e| Error::with(e, ErrorKind::Process));
            let task = task.and_then(move |output| {
                debug!(log, "subprocess: detach_abort finished");
                log_process_output(&log, &output);

                *state.borrow_mut() = State::Normal;
                Ok(())
            });

            self.schedule_process_task(Box::new(task))
        } else {
            let log = self.log.clone();
            let state = self.state.clone();
            let task = future::lazy(move || {
                debug!(log, "subprocess: no detach_abort handler executable");

                *state.borrow_mut() = State::Normal;
                Ok(())
            });

            self.schedule_process_task(Box::new(task))
        }
    }

    fn schedule_process_task(&mut self, task: BoxedTask) -> Result<()> {
        let res = self.task_queue_tx.try_send(Box::new(task));
        if let Err(e) = res {
            if e.is_full() {
                warn!(self.log, "process queue is full, dropping task");
            } else {
                unreachable!("process queue closed");
            }
        }

        Ok(())
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State { Normal, Detaching, Aborting, Attaching }

fn log_process_output(log: &Logger, output: &std::process::Output) {
    if !output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty() {
        info!(log, "subprocess terminated with {}", output.status);
    }

    if !output.stdout.is_empty() {
        let stdout = OsStr::from_bytes(&output.stdout);
        info!(log, "subprocess terminated with stdout: {:?}", stdout);
    }

    if !output.stderr.is_empty() {
        let stderr = OsStr::from_bytes(&output.stderr);
        info!(log, "subprocess terminated with stderr: {:?}", stderr);
    }
}

fn panic_with_critical_error(log: &Logger, err: &Error) -> ! {
    crit!(log, "Error: {}", err);
    for cause in err.iter_causes() {
        crit!(log, "Caused by: {}", cause);
    }

    panic!(format!("{}", err))
}
